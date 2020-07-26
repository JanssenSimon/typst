//! Parsing of source code into syntax models.

use std::iter::FromIterator;
use std::str::FromStr;

use crate::{Pass, Feedback};
use super::func::{FuncHeader, FuncArgs, FuncArg};
use super::expr::*;
use super::scope::Scope;
use super::span::{Position, Span, Spanned};
use super::tokens::{Token, Tokens, TokenizationMode};
use super::*;


/// The context for parsing.
#[derive(Debug, Copy, Clone)]
pub struct ParseContext<'a> {
    /// The scope containing function definitions.
    pub scope: &'a Scope,
}

/// Parse source code into a syntax model.
///
/// All errors and decorations are offset by the `start` position.
pub fn parse(start: Position, src: &str, ctx: ParseContext) -> Pass<SyntaxModel> {
    let mut model = SyntaxModel::new();
    let mut feedback = Feedback::new();

    // We always start in body mode. The header tokenization mode is only used
    // in the `FuncParser`.
    let mut tokens = Tokens::new(start, src, TokenizationMode::Body);

    while let Some(token) = tokens.next() {
        let span = token.span;

        let node = match token.v {
            Token::LineComment(_) | Token::BlockComment(_) => continue,

            // Only at least two newlines mean a _real_ newline indicating a
            // paragraph break.
            Token::Space(newlines) => if newlines >= 2 {
                Node::Parbreak
            } else {
                Node::Space
            },

            Token::Function { header, body, terminated } => {
                let parsed = FuncParser::new(header, body, ctx).parse();
                feedback.extend_offset(span.start, parsed.feedback);

                if !terminated {
                    error!(@feedback, Span::at(span.end), "expected closing bracket");
                }

                parsed.output
            }

            Token::Star       => Node::ToggleBolder,
            Token::Underscore => Node::ToggleItalic,
            Token::Backslash  => Node::Linebreak,

            Token::Raw { raw, terminated } => {
                if !terminated {
                    error!(@feedback, Span::at(span.end), "expected backtick");
                }

                Node::Raw(unescape_raw(raw))
            }

            Token::Text(text) => Node::Text(text.to_string()),

            other => {
                error!(@feedback, span, "unexpected {}", other.name());
                continue;
            }
        };

        model.add(Spanned { v: node, span: token.span });
    }

    Pass::new(model, feedback)
}

/// Performs the function parsing.
struct FuncParser<'s> {
    ctx: ParseContext<'s>,
    feedback: Feedback,

    /// ```typst
    /// [tokens][body]
    ///  ^^^^^^
    /// ```
    tokens: Tokens<'s>,
    peeked: Option<Option<Spanned<Token<'s>>>>,

    /// The spanned body string if there is a body.
    /// ```typst
    /// [tokens][body]
    ///          ^^^^
    /// ```
    body: Option<Spanned<&'s str>>,
}

impl<'s> FuncParser<'s> {
    /// Create a new function parser.
    fn new(
        header: &'s str,
        body: Option<Spanned<&'s str>>,
        ctx: ParseContext<'s>
    ) -> FuncParser<'s> {
        FuncParser {
            ctx,
            feedback: Feedback::new(),
            tokens: Tokens::new(Position::new(0, 1), header, TokenizationMode::Header),
            peeked: None,
            body,
        }
    }

    /// Do the parsing.
    fn parse(mut self) -> Pass<Node> {
        let parsed = if let Some(header) = self.parse_func_header() {
            let name = header.name.v.as_str();
            let (parser, deco) = match self.ctx.scope.get_parser(name) {
                // A valid function.
                Ok(parser) => (parser, Decoration::ValidFuncName),

                // The fallback parser was returned. Invalid function.
                Err(parser) => {
                    error!(@self.feedback, header.name.span, "unknown function");
                    (parser, Decoration::InvalidFuncName)
                }
            };

            self.feedback.decos.push(Spanned::new(deco, header.name.span));

            parser(header, self.body, self.ctx)
        } else {
            let default = FuncHeader {
                name: Spanned::new(Ident("".to_string()), Span::ZERO),
                args: FuncArgs::new(),
            };

            // Use the fallback function such that the body is still rendered
            // even if the header is completely unparsable.
            self.ctx.scope.get_fallback_parser()(default, self.body, self.ctx)
        };

        self.feedback.extend(parsed.feedback);

        Pass::new(Node::Model(parsed.output), self.feedback)
    }

    /// Parse the header tokens.
    fn parse_func_header(&mut self) -> Option<FuncHeader> {
        let start = self.pos();
        self.skip_whitespace();

        let name = match self.parse_ident() {
            Some(ident) => ident,
            None => {
                let other = self.eat();
                self.expected_found_or_at("identifier", other, start);
                return None;
            }
        };

        self.skip_whitespace();
        let args = match self.eat().map(Spanned::value) {
            Some(Token::Colon) => self.parse_func_args(),
            Some(_) => {
                self.expected_at("colon", name.span.end);
                FuncArgs::new()
            }
            None => FuncArgs::new(),
        };

        Some(FuncHeader { name, args })
    }

    /// Parse the argument list between colons and end of the header.
    fn parse_func_args(&mut self) -> FuncArgs {
        // Parse a collection until the token is `None`, that is, the end of the
        // header.
        self.parse_collection(None, |p| {
            // If we have an identifier we might have a keyword argument,
            // otherwise its for sure a postional argument.
            if let Some(ident) = p.parse_ident() {
                // This could still be a named tuple
                if let Some(Token::LeftParen) = p.peekv() {
                    let tuple = p.parse_named_tuple(ident);
                    return Ok(tuple.map(|t| FuncArg::Pos(Expr::NamedTuple(t))));
                }

                p.skip_whitespace();

                if let Some(Token::Equals) = p.peekv() {
                    p.eat();
                    p.skip_whitespace();

                    // Semantic highlighting for argument keys.
                    p.feedback.decos.push(
                        Spanned::new(Decoration::ArgumentKey, ident.span));

                    let value = p.parse_expr().ok_or(("value", None))?;

                    // Add a keyword argument.
                    let span = Span::merge(ident.span, value.span);
                    let pair = Pair { key: ident, value };
                    Ok(Spanned::new(FuncArg::Key(pair), span))
                } else {
                    // Add a positional argument because there was no equals
                    // sign after the identifier that could have been a key.
                    Ok(ident.map(|id| FuncArg::Pos(Expr::Ident(id))))
                }
            } else {
                // Add a positional argument because we haven't got an
                // identifier that could be an argument key.
                let value = p.parse_expr().ok_or(("argument", None))?;
                Ok(value.map(|expr| FuncArg::Pos(expr)))
            }
        }).v
    }

    /// Parse an expression which may contain math operands. For this, this
    /// method looks for operators in descending order of associativity, i.e. we
    /// first drill down to find all negations, brackets and tuples, the next
    /// level, we look for multiplication and division and here finally, for
    /// addition and subtraction.
    fn parse_expr(&mut self) -> Option<Spanned<Expr>> {
        let o1 = self.parse_term()?;
        self.parse_binop(o1, "summand", Self::parse_expr, |token| match token {
            Token::Plus => Some(Expr::Add),
            Token::Hyphen => Some(Expr::Sub),
            _ => None,
        })
    }

    fn parse_term(&mut self) -> Option<Spanned<Expr>> {
        let o1 = self.parse_factor()?;
        self.parse_binop(o1, "factor", Self::parse_term, |token| match token {
            Token::Star => Some(Expr::Mul),
            Token::Slash => Some(Expr::Div),
            _ => None,
        })
    }

    fn parse_binop<F, G>(
        &mut self,
        o1: Spanned<Expr>,
        operand_name: &str,
        parse_operand: F,
        parse_op: G,
    ) -> Option<Spanned<Expr>>
    where
        F: FnOnce(&mut Self) -> Option<Spanned<Expr>>,
        G: FnOnce(Token) -> Option<fn(Box<Spanned<Expr>>, Box<Spanned<Expr>>) -> Expr>,
    {
        self.skip_whitespace();

        if let Some(next) = self.peek() {
            if let Some(binop) = parse_op(next.v) {
                self.eat();
                self.skip_whitespace();

                if let Some(o2) = parse_operand(self) {
                    let span = Span::merge(o1.span, o2.span);
                    let expr = binop(Box::new(o1), Box::new(o2));
                    return Some(Spanned::new(expr, span));
                } else {
                    error!(
                        @self.feedback, Span::merge(next.span, o1.span),
                        "missing right {}", operand_name,
                    );
                }
            }
        }

        Some(o1)
    }

    /// Parse expressions that are of the form value or -value.
    fn parse_factor(&mut self) -> Option<Spanned<Expr>> {
        let first = self.peek()?;
        if first.v == Token::Hyphen {
            self.eat();
            self.skip_whitespace();

            if let Some(factor) = self.parse_value() {
                let span = Span::merge(first.span, factor.span);
                Some(Spanned::new(Expr::Neg(Box::new(factor)), span))
            } else {
                error!(@self.feedback, first.span, "dangling minus");
                None
            }
        } else {
            self.parse_value()
        }
    }

    fn parse_value(&mut self) -> Option<Spanned<Expr>> {
        let first = self.peek()?;
        macro_rules! take {
            ($v:expr) => ({ self.eat(); Spanned { v: $v, span: first.span } });
        }

        Some(match first.v {
            Token::ExprIdent(i) => {
                let name = take!(Ident(i.to_string()));

                // This could be a named tuple or an identifier
                if let Some(Token::LeftParen) = self.peekv() {
                    self.parse_named_tuple(name).map(|t| Expr::NamedTuple(t))
                } else {
                    name.map(|i| Expr::Ident(i))
                }
            },
            Token::ExprStr { string, terminated } => {
                if !terminated {
                    self.expected_at("quote", first.span.end);
                }

                take!(Expr::Str(unescape_string(string)))
            }

            Token::ExprNumber(n) => take!(Expr::Number(n)),
            Token::ExprSize(s) => take!(Expr::Size(s)),
            Token::ExprBool(b) => take!(Expr::Bool(b)),
            Token::ExprHex(s) => {
                if let Ok(color) = RgbaColor::from_str(s) {
                    take!(Expr::Color(color))
                } else {
                    // Heal color by assuming black
                    error!(@self.feedback, first.span, "invalid color");
                    take!(Expr::Color(RgbaColor::new_healed(0, 0, 0, 255)))
                }
            },

            Token::LeftParen => {
                let (mut tuple, can_be_coerced) = self.parse_tuple();
                // Coerce 1-tuple into value
                if can_be_coerced && tuple.v.items.len() > 0 {
                    tuple.v.items.pop().expect("length is at least one")
                } else {
                    tuple.map(|t| Expr::Tuple(t))
                }
            },
            Token::LeftBrace => self.parse_object().map(|o| Expr::Object(o)),

            _ => return None,
        })
    }

    /// Parse a tuple expression: `(<expr>, ...)`. The boolean in the return
    /// values showes whether the tuple can be coerced into a single value.
    fn parse_tuple(&mut self) -> (Spanned<Tuple>, bool) {
        let token = self.eat();
        debug_assert_eq!(token.map(Spanned::value), Some(Token::LeftParen));

        // Parse a collection until a right paren appears and complain about
        // missing a `value` when an invalid token is encoutered.
        self.parse_collection_comma_aware(Some(Token::RightParen),
            |p| p.parse_expr().ok_or(("value", None)))
    }

    /// Parse a tuple expression: `name(<expr>, ...)` with a given identifier.
    fn parse_named_tuple(&mut self, name: Spanned<Ident>) -> Spanned<NamedTuple> {
        let tuple = self.parse_tuple().0;
        let span = Span::merge(name.span, tuple.span);
        Spanned::new(NamedTuple::new(name, tuple), span)
    }

    /// Parse an object expression: `{ <key>: <value>, ... }`.
    fn parse_object(&mut self) -> Spanned<Object> {
        let token = self.eat();
        debug_assert_eq!(token.map(Spanned::value), Some(Token::LeftBrace));

        // Parse a collection until a right brace appears.
        self.parse_collection(Some(Token::RightBrace), |p| {
            // Expect an identifier as the key.
            let key = p.parse_ident().ok_or(("key", None))?;

            // Expect a colon behind the key (only separated by whitespace).
            let behind_key = p.pos();
            p.skip_whitespace();
            if p.peekv() != Some(Token::Colon) {
                return Err(("colon", Some(behind_key)));
            }

            p.eat();
            p.skip_whitespace();

            // Semantic highlighting for object keys.
            p.feedback.decos.push(
                Spanned::new(Decoration::ObjectKey, key.span));

            let value = p.parse_expr().ok_or(("value", None))?;

            let span = Span::merge(key.span, value.span);
            Ok(Spanned::new(Pair { key, value }, span))
        })
    }

    /// Parse a comma-separated collection where each item is parsed through
    /// `parse_item` until the `end` token is met.
    fn parse_collection<C, I, F>(
        &mut self,
        end: Option<Token>,
        parse_item: F
    ) -> Spanned<C>
    where
        C: FromIterator<Spanned<I>>,
        F: FnMut(&mut Self) -> Result<Spanned<I>, (&'static str, Option<Position>)>,
    {
        self.parse_collection_comma_aware(end, parse_item).0
    }

    /// Parse a comma-separated collection where each item is parsed through
    /// `parse_item` until the `end` token is met. The first item in the return
    /// tuple is the collection, the second item indicates whether the
    /// collection can be coerced into a single item (i.e. no comma appeared).
    fn parse_collection_comma_aware<C, I, F>(
        &mut self,
        end: Option<Token>,
        mut parse_item: F
    ) -> (Spanned<C>, bool)
    where
        C: FromIterator<Spanned<I>>,
        F: FnMut(&mut Self) -> Result<Spanned<I>, (&'static str, Option<Position>)>,
    {
        let start = self.pos();
        let mut can_be_coerced = true;

        // Parse the comma separated items.
        let collection = std::iter::from_fn(|| {
            self.skip_whitespace();
            let peeked = self.peekv();

            // We finished as expected.
            if peeked == end {
                self.eat();
                return None;
            }

            // We finished without the expected end token (which has to be a
            // `Some` value at this point since otherwise we would have already
            // returned in the previous case).
            if peeked == None {
                self.eat();
                self.expected_at(end.unwrap().name(), self.pos());
                return None;
            }

            // Try to parse a collection item.
            match parse_item(self) {
                Ok(item) => {
                    // Expect a comma behind the item (only separated by
                    // whitespace).
                    self.skip_whitespace();
                    match self.peekv() {
                        Some(Token::Comma) => {
                            can_be_coerced = false;
                            self.eat();
                        }
                        t @ Some(_) if t != end => {
                            can_be_coerced = false;
                            self.expected_at("comma", item.span.end);
                        },
                        _ => {}
                    }

                    return Some(Some(item));
                }

                // The item parser expected something different at either some
                // given position or instead of the currently peekable token.
                Err((expected, Some(pos))) => self.expected_at(expected, pos),
                Err((expected, None)) => {
                    let token = self.peek();
                    if token.map(Spanned::value) != end {
                        self.eat();
                    }
                    self.expected_found_or_at(expected, token, self.pos());
                }
            }

            Some(None)
        }).filter_map(|x| x).collect();

        let end = self.pos();
        (Spanned::new(collection, Span { start, end }), can_be_coerced)
    }

    /// Try to parse an identifier and do nothing if the peekable token is no
    /// identifier.
    fn parse_ident(&mut self) -> Option<Spanned<Ident>> {
        match self.peek() {
            Some(Spanned { v: Token::ExprIdent(s), span }) => {
                self.eat();
                Some(Spanned { v: Ident(s.to_string()), span })
            }
            _ => None
        }
    }

    /// Skip all whitespace/comment tokens.
    fn skip_whitespace(&mut self) {
        self.eat_until(|t| match t {
            Token::Space(_) | Token::LineComment(_) |
            Token::BlockComment(_) => false,
            _ => true,
        }, false)
    }

    /// Add an error about an expected `thing` which was not found, showing
    /// what was found instead.
    fn expected_found(&mut self, thing: &str, found: Spanned<Token>) {
        error!(
            @self.feedback, found.span,
            "expected {}, found {}", thing, found.v.name(),
        );
    }

    /// Add an error about an `thing` which was expected but not found at the
    /// given position.
    fn expected_at(&mut self, thing: &str, pos: Position) {
        error!(@self.feedback, Span::at(pos), "expected {}", thing);
    }

    /// Add a expected-found-error if `found` is `Some` and an expected-error
    /// otherwise.
    fn expected_found_or_at(
        &mut self,
        thing: &str,
        found: Option<Spanned<Token>>,
        pos: Position
    ) {
        match found {
            Some(found) => self.expected_found(thing, found),
            None => self.expected_at(thing, pos),
        }
    }

    /// Consume tokens until the function returns true and only consume the last
    /// token if instructed to so by `eat_match`.
    fn eat_until<F>(&mut self, mut f: F, eat_match: bool)
    where F: FnMut(Token<'s>) -> bool {
        while let Some(token) = self.peek() {
            if f(token.v) {
                if eat_match {
                    self.eat();
                }
                break;
            }

            self.eat();
        }
    }

    /// Consume and return the next token.
    fn eat(&mut self) -> Option<Spanned<Token<'s>>> {
        self.peeked.take()
            .unwrap_or_else(|| self.tokens.next())
    }

    /// Peek at the next token without consuming it.
    fn peek(&mut self) -> Option<Spanned<Token<'s>>> {
        let iter = &mut self.tokens;
        *self.peeked.get_or_insert_with(|| iter.next())
    }

    /// Peek at the unspanned value of the next token.
    fn peekv(&mut self) -> Option<Token<'s>> {
        self.peek().map(Spanned::value)
    }

    /// The position at the end of the last eaten token / start of the peekable
    /// token.
    fn pos(&self) -> Position {
        self.peeked.flatten()
            .map(|s| s.span.start)
            .unwrap_or_else(|| self.tokens.pos())
    }
}

/// Unescape a string: `the string is \"this\"` => `the string is "this"`.
fn unescape_string(string: &str) -> String {
    let mut s = String::with_capacity(string.len());
    let mut iter = string.chars();

    while let Some(c) = iter.next() {
        if c == '\\' {
            match iter.next() {
                Some('\\') => s.push('\\'),
                Some('"') => s.push('"'),
                Some('n') => s.push('\n'),
                Some('t') => s.push('\t'),
                Some(c) => { s.push('\\'); s.push(c); }
                None => s.push('\\'),
            }
        } else {
            s.push(c);
        }
    }

    s
}

/// Unescape raw markup into lines.
fn unescape_raw(raw: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut s = String::new();
    let mut iter = raw.chars().peekable();

    while let Some(c) = iter.next() {
        if c == '\\' {
            match iter.next() {
                Some('`') => s.push('`'),
                Some(c) => { s.push('\\'); s.push(c); }
                None => s.push('\\'),
            }
        } else if is_newline_char(c) {
            if c == '\r' && iter.peek() == Some(&'\n') {
                iter.next();
            }

            lines.push(std::mem::replace(&mut s, String::new()));
        } else {
            s.push(c);
        }
    }

    lines.push(s);
    lines
}


#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use crate::size::Size;
    use crate::syntax::test::{DebugFn, check};
    use crate::syntax::func::Value;
    use super::*;

    use Decoration::*;
    use Expr::{Number as Num, Size as Sz, Bool};
    use Node::{
        Space as S, ToggleItalic as Italic, ToggleBolder as Bold,
        Parbreak, Linebreak,
    };

    /// Test whether the given string parses into
    /// - the given node list (required).
    /// - the given error list (optional, if omitted checks against empty list).
    /// - the given decoration list (optional, if omitted it is not tested).
    macro_rules! p {
        ($source:expr => [$($model:tt)*]) => {
            p!($source => [$($model)*], []);
        };

        ($source:expr => [$($model:tt)*], [$($problems:tt)*] $(, [$($decos:tt)*])? $(,)?) => {
            let mut scope = Scope::new::<DebugFn>();
            scope.add::<DebugFn>("f");
            scope.add::<DebugFn>("n");
            scope.add::<DebugFn>("box");
            scope.add::<DebugFn>("val");

            let ctx = ParseContext { scope: &scope };
            let pass = parse(Position::ZERO, $source, ctx);

            // Test model.
            let (exp, cmp) = span_vec![$($model)*];
            check($source, exp, pass.output.nodes, cmp);

            // Test problems.
            let (exp, cmp) = span_vec![$($problems)*];
            let exp = exp.into_iter()
                .map(|s: Spanned<&str>| s.map(|e| e.to_string()))
                .collect::<Vec<_>>();
            let found = pass.feedback.problems.into_iter()
                .map(|s| s.map(|e| e.message))
                .collect::<Vec<_>>();
            check($source, exp, found, cmp);

            // Test decos.
            $(let (exp, cmp) = span_vec![$($decos)*];
            check($source, exp, pass.feedback.decos, cmp);)?
        };
    }

    /// Shorthand for `p!("[val: ...]" => func!("val", ...))`.
    macro_rules! pval {
        ($header:expr => $($tts:tt)*) => {
            p!(concat!("[val: ", $header, "]") => [func!("val": $($tts)*)]);
        }
    }

    fn Id(text: &str) -> Expr { Expr::Ident(Ident(text.to_string())) }
    fn Str(text: &str) -> Expr { Expr::Str(text.to_string()) }
    fn Pt(points: f32) -> Expr { Expr::Size(Size::pt(points)) }
    fn Color(r: u8, g: u8, b: u8, a: u8) -> Expr { Expr::Color(RgbaColor::new(r, g, b, a)) }
    fn ColorStr(color: &str) -> Expr { Expr::Color(RgbaColor::from_str(color).expect("invalid test color")) }
    fn ColorHealed() -> Expr { Expr::Color(RgbaColor::new_healed(0, 0, 0, 255)) }
    fn Neg(e1: Expr) -> Expr { Expr::Neg(Box::new(Z(e1))) }
    fn Add(e1: Expr, e2: Expr) -> Expr { Expr::Add(Box::new(Z(e1)), Box::new(Z(e2))) }
    fn Sub(e1: Expr, e2: Expr) -> Expr { Expr::Sub(Box::new(Z(e1)), Box::new(Z(e2))) }
    fn Mul(e1: Expr, e2: Expr) -> Expr { Expr::Mul(Box::new(Z(e1)), Box::new(Z(e2))) }
    fn Div(e1: Expr, e2: Expr) -> Expr { Expr::Div(Box::new(Z(e1)), Box::new(Z(e2)))  }
    fn T(text: &str) -> Node { Node::Text(text.to_string()) }
    fn Z<T>(v: T) -> Spanned<T> { Spanned::zero(v) }

    macro_rules! tuple {
        ($($items:expr),* $(,)?) => {
            Expr::Tuple(Tuple { items: span_vec![$($items),*].0 })
        };
    }

    macro_rules! named_tuple {
        ($name:expr $(, $items:expr)* $(,)?) => {
            Expr::NamedTuple(NamedTuple::new(
                Z(Ident($name.to_string())),
                Z(Tuple { items: span_vec![$($items),*].0 })
            ))
        };
    }

    macro_rules! object {
        ($($key:expr => $value:expr),* $(,)?) => {
            Expr::Object(Object {
                pairs: vec![$(Z(Pair {
                    key: Z(Ident($key.to_string())),
                    value: Z($value),
                })),*]
            })
        };
    }

    macro_rules! raw {
        ($($line:expr),* $(,)?) => {
            Node::Raw(vec![$($line.to_string()),*])
        };
    }

    macro_rules! func {
        ($name:tt $(: ($($pos:tt)*) $(, { $($key:tt)* })? )? $(; $($body:tt)*)?) => {{
            #[allow(unused_mut)]
            let mut args = FuncArgs::new();
            $(args.pos = Tuple::parse(Z(tuple!($($pos)*))).unwrap();
            $(args.key = Object::parse(Z(object! { $($key)* })).unwrap();)?)?
            Node::Model(Box::new(DebugFn {
                header: FuncHeader {
                    name: span_item!($name).map(|s| Ident(s.to_string())),
                    args,
                },
                body: func!(@body $($($body)*)?),
            }))
        }};
        (@body [$($body:tt)*]) => { Some(SyntaxModel { nodes: span_vec![$($body)*].0 }) };
        (@body) => { None };
    }

    #[test]
    fn parse_color_strings() {
        assert_eq!(Color(0xf6, 0x12, 0x43, 0xff), ColorStr("f61243ff"));
        assert_eq!(Color(0xb3, 0xd8, 0xb3, 0xff), ColorStr("b3d8b3"));
        assert_eq!(Color(0xfc, 0xd2, 0xa9, 0xad), ColorStr("fCd2a9AD"));
        assert_eq!(Color(0x22, 0x33, 0x33, 0xff), ColorStr("233"));
        assert_eq!(Color(0x11, 0x11, 0x11, 0xbb), ColorStr("111b"));
    }

    #[test]
    fn unescape_strings() {
        fn test(string: &str, expected: &str) {
            assert_eq!(unescape_string(string), expected.to_string());
        }

        test(r#"hello world"#,  "hello world");
        test(r#"hello\nworld"#, "hello\nworld");
        test(r#"a\"bc"#,        "a\"bc");
        test(r#"a\\"#,          "a\\");
        test(r#"a\\\nbc"#,      "a\\\nbc");
        test(r#"a\tbc"#,        "a\tbc");
        test(r"🌎",             "🌎");
        test(r"🌎\",            r"🌎\");
        test(r"\🌎",            r"\🌎");
    }

    #[test]
    fn unescape_raws() {
        fn test(raw: &str, expected: Node) {
            let vec = if let Node::Raw(v) = expected { v } else { panic!() };
            assert_eq!(unescape_raw(raw), vec);
        }

        test("raw\\`",     raw!["raw`"]);
        test("raw\ntext",  raw!["raw", "text"]);
        test("a\r\nb",     raw!["a", "b"]);
        test("a\n\nb",     raw!["a", "", "b"]);
        test("a\r\x0Bb",   raw!["a", "", "b"]);
        test("a\r\n\r\nb", raw!["a", "", "b"]);
        test("raw\\a",     raw!["raw\\a"]);
        test("raw\\",      raw!["raw\\"]);
    }

    #[test]
    fn parse_basic_nodes() {
        // Basic nodes.
        p!(""                     => []);
        p!("hi"                   => [T("hi")]);
        p!("*hi"                  => [Bold, T("hi")]);
        p!("hi_"                  => [T("hi"), Italic]);
        p!("hi you"               => [T("hi"), S, T("you")]);
        p!("hi// you\nw"          => [T("hi"), S, T("w")]);
        p!("\n\n\nhello"          => [Parbreak, T("hello")]);
        p!("first//\n//\nsecond"  => [T("first"), S, S, T("second")]);
        p!("first//\n \nsecond"   => [T("first"), Parbreak, T("second")]);
        p!("first/*\n \n*/second" => [T("first"), T("second")]);
        p!(r"a\ b"                => [T("a"), Linebreak, S, T("b")]);
        p!("💜\n\n 🌍"            => [T("💜"), Parbreak, T("🌍")]);

        // Raw markup.
        p!("`py`"         => [raw!["py"]]);
        p!("[val][`hi]`]" => [func!("val"; [raw!["hi]"]])]);
        p!("`hi\nyou"     => [raw!["hi", "you"]], [(1:3, 1:3, "expected backtick")]);
        p!("`hi\\`du`"    => [raw!["hi`du"]]);

        // Spanned nodes.
        p!("Hi"      => [(0:0, 0:2, T("Hi"))]);
        p!("*Hi*"    => [(0:0, 0:1, Bold), (0:1, 0:3, T("Hi")), (0:3, 0:4, Bold)]);
        p!("🌎\n*/[n]" =>
            [(0:0, 0:1, T("🌎")), (0:1, 1:0, S), (1:2, 1:5, func!((0:1, 0:2, "n")))],
            [(1:0, 1:2, "unexpected end of block comment")],
            [(1:3, 1:4, ValidFuncName)],
        );
    }

    #[test]
    fn parse_function_names() {
        // No closing bracket.
        p!("[" => [func!("")], [
            (0:1, 0:1, "expected identifier"),
            (0:1, 0:1, "expected closing bracket")
        ]);

        // No name.
        p!("[]" => [func!("")], [(0:1, 0:1, "expected identifier")]);
        p!("[\"]" => [func!("")], [
            (0:1, 0:3, "expected identifier, found string"),
            (0:3, 0:3, "expected closing bracket"),
        ]);

        // An unknown name.
        p!("[hi]" =>
            [func!("hi")],
            [(0:1, 0:3, "unknown function")],
            [(0:1, 0:3, InvalidFuncName)],
        );

        // A valid name.
        p!("[f]"   => [func!("f")], [], [(0:1, 0:2, ValidFuncName)]);
        p!("[  f]" => [func!("f")], [], [(0:3, 0:4, ValidFuncName)]);

        // An invalid token for a name.
        p!("[12]"   => [func!("")], [(0:1, 0:3, "expected identifier, found number")], []);
        p!("[🌎]"   => [func!("")], [(0:1, 0:2, "expected identifier, found invalid token")], []);
        p!("[  🌎]" => [func!("")], [(0:3, 0:4, "expected identifier, found invalid token")], []);
    }

    #[test]
    fn parse_colon_starting_function_arguments() {
        // Valid.
        p!("[val: true]" =>
            [func!["val": (Bool(true))]], [],
            [(0:1, 0:4, ValidFuncName)],
        );

        // No colon before arg.
        p!("[val\"s\"]" => [func!("val")], [(0:4, 0:4, "expected colon")]);

        // No colon before valid, but wrong token.
        p!("[val=]" => [func!("val")], [(0:4, 0:4, "expected colon")]);

        // No colon before invalid tokens, which are ignored.
        p!("[val/🌎:$]" =>
            [func!("val")],
            [(0:4, 0:4, "expected colon")],
            [(0:1, 0:4, ValidFuncName)],
        );

        // String in invalid header without colon still parsed as string
        // Note: No "expected quote" error because not even the string was
        //       expected.
        p!("[val/\"]" => [func!("val")], [
            (0:4, 0:4, "expected colon"),
            (0:7, 0:7, "expected closing bracket"),
        ]);

        // Just colon without args.
        p!("[val:]"         => [func!("val")]);
        p!("[val:/*12pt*/]" => [func!("val")]);

        // Whitespace / comments around colon.
        p!("[val\n:\ntrue]"      => [func!("val": (Bool(true)))]);
        p!("[val/*:*/://\ntrue]" => [func!("val": (Bool(true)))]);
    }

    #[test]
    fn parse_one_positional_argument() {
        // Different expressions.
        pval!("_"      => (Id("_")));
        pval!("name"   => (Id("name")));
        pval!("\"hi\"" => (Str("hi")));
        pval!("3.14"   => (Num(3.14)));
        pval!("4.5cm"  => (Sz(Size::cm(4.5))));
        pval!("12e1pt" => (Pt(12e1)));
        pval!("#f7a20500" => (ColorStr("f7a20500")));
        pval!("\"a\n[]\\\"string\"" => (Str("a\n[]\"string")));

        // Trailing comma.
        pval!("a," => (Id("a")));

        // Simple coerced tuple.
        pval!("(hi)" => (Id("hi")));

        // Math.
        pval!("3.2in + 6pt" => (Add(Sz(Size::inches(3.2)), Sz(Size::pt(6.0)))));
        pval!("5 - 0.01"    => (Sub(Num(5.0), Num(0.01))));
        pval!("(3mm * 2)"   => (Mul(Sz(Size::mm(3.0)), Num(2.0))));
        pval!("12e-3cm/1pt" => (Div(Sz(Size::cm(12e-3)), Sz(Size::pt(1.0)))));

        // Unclosed string.
        p!("[val: \"hello]" => [func!("val": (Str("hello]")), {})], [
            (0:13, 0:13, "expected quote"),
            (0:13, 0:13, "expected closing bracket"),
        ]);

        // Invalid, healed colors.
        p!("[val: #12345]"     => [func!("val": (ColorHealed()))], [(0:6, 0:12, "invalid color")]);
        p!("[val: #a5]"        => [func!("val": (ColorHealed()))], [(0:6, 0:9,  "invalid color")]);
        p!("[val: #14b2ah]"    => [func!("val": (ColorHealed()))], [(0:6, 0:13, "invalid color")]);
        p!("[val: #f075ff011]" => [func!("val": (ColorHealed()))], [(0:6, 0:16, "invalid color")]);
    }

    #[test]
    fn parse_complex_mathematical_expressions() {
        // Valid expressions.
        pval!("(3.2in + 6pt)*(5/2-1)" => (Mul(
            Add(Sz(Size::inches(3.2)), Sz(Size::pt(6.0))),
            Sub(Div(Num(5.0), Num(2.0)), Num(1.0))
        )));
        pval!("(6.3E+2+4* - 3.2pt)/2" => (Div(
            Add(Num(6.3e2),Mul(Num(4.0), Neg(Pt(3.2)))),
            Num(2.0)
        )));

        // Invalid expressions.
        p!("[val: 4pt--]" => [func!("val": (Pt(4.0)))], [
            (0:10, 0:11, "dangling minus"),
            (0:6, 0:10, "missing right summand")
        ]);
        p!("[val: 3mm+4pt*]" =>
            [func!("val": (Add(Sz(Size::mm(3.0)), Pt(4.0))))],
            [(0:10, 0:14, "missing right factor")],
        );
    }

    #[test]
    fn parse_tuples() {
        // Empty tuple.
        pval!("()" => (tuple!()));
        pval!("empty()" => (named_tuple!("empty")));

        // Invalid value.
        p!("[val: sound(\x07)]" =>
            [func!("val": (named_tuple!("sound")), {})],
            [(0:12, 0:13, "expected value, found invalid token")],
        );

        // Invalid tuple name.
        p!("[val: 👠(\"abc\", 13e-5)]" =>
            [func!("val": (tuple!(Str("abc"), Num(13.0e-5))), {})],
            [(0:6, 0:7, "expected argument, found invalid token")],
        );

        // Unclosed tuple.
        p!("[val: lang(中文]" =>
            [func!("val": (named_tuple!("lang", Id("中文"))), {})],
            [(0:13, 0:13, "expected closing paren")],
        );

        // Valid values.
        pval!("(1, 2)" => (tuple!(Num(1.0), Num(2.0))));
        pval!("(\"s\",)" => (tuple!(Str("s"))));
        pval!("items(\"fire\", #f93a6d)" => (
            named_tuple!("items", Str("fire"), ColorStr("f93a6d")
        )));

        // Nested tuples.
        pval!("css(1pt, rgb(90, 102, 254), \"solid\")" => (named_tuple!(
            "css",
            Pt(1.0),
            named_tuple!("rgb", Num(90.0), Num(102.0), Num(254.0)),
            Str("solid"),
        )));

        // Invalid commas.
        p!("[val: (,)]" =>
            [func!("val": (tuple!()), {})],
            [(0:7, 0:8, "expected value, found comma")],
        );
        p!("[val: (true false)]" =>
            [func!("val": (tuple!(Bool(true), Bool(false))), {})],
            [(0:11, 0:11, "expected comma")],
        );
    }

    #[test]
    fn parse_objects() {
        let val = || func!("val": (object! {}), {});

        // Okay objects.
        pval!("{}" => (object! {}));
        pval!("{ key: value }" => (object! { "key" => Id("value") }));

        // Unclosed object.
        p!("[val: {hello: world]" =>
            [func!("val": (object! { "hello" => Id("world") }), {})],
            [(0:19, 0:19, "expected closing brace")],
        );
        p!("[val: { a]" =>
            [func!("val": (object! {}), {})],
            [(0:9, 0:9, "expected colon"), (0:9, 0:9, "expected closing brace")],
        );

        // Missing key.
        p!("[val: {,}]" => [val()], [(0:7, 0:8, "expected key, found comma")]);
        p!("[val: { 12pt }]" => [val()], [(0:8, 0:12, "expected key, found size")]);
        p!("[val: { : }]" => [val()], [(0:8, 0:9, "expected key, found colon")]);

        // Missing colon.
        p!("[val: { key }]" => [val()], [(0:11, 0:11, "expected colon")]);
        p!("[val: { key false }]" => [val()], [
            (0:11, 0:11, "expected colon"),
            (0:12, 0:17, "expected key, found bool"),
        ]);
        p!("[val: { a b:c }]" =>
            [func!("val": (object! { "b" => Id("c") }), {})],
            [(0:9, 0:9, "expected colon")],
        );

        // Missing value.
        p!("[val: { key: : }]" => [val()], [(0:13, 0:14, "expected value, found colon")]);
        p!("[val: { key: , k: \"s\" }]" =>
            [func!("val": (object! { "k" => Str("s") }), {})],
            [(0:13, 0:14, "expected value, found comma")],
        );

        // Missing comma, invalid token.
        p!("[val: left={ a: 2, b: false 🌎 }]" =>
            [func!("val": (), {
                "left" => object! {
                    "a" => Num(2.0),
                    "b" => Bool(false),
                }
            })],
            [(0:27, 0:27, "expected comma"),
             (0:28, 0:29, "expected key, found invalid token")],
        );
    }

    #[test]
    fn parse_nested_tuples_and_objects() {
        pval!("(1, { ab: (), d: (3, 14pt) }), false" => (
            tuple!(
                Num(1.0),
                object!(
                    "ab" => tuple!(),
                    "d" => tuple!(Num(3.0), Pt(14.0)),
                ),
            ),
            Bool(false),
        ));
    }

    #[test]
    fn parse_one_keyword_argument() {
        // Correct
        p!("[val: x=true]" =>
            [func!("val": (), { "x" => Bool(true) })], [],
            [(0:6, 0:7, ArgumentKey), (0:1, 0:4, ValidFuncName)],
        );

        // Spacing around keyword arguments
        p!("\n [val: \n hi \n = /* //\n */ \"s\n\"]" =>
            [S, func!("val": (), { "hi" => Str("s\n") })], [],
            [(2:1, 2:3, ArgumentKey), (1:2, 1:5, ValidFuncName)],
        );

        // Missing value
        p!("[val: x=]" =>
            [func!("val")],
            [(0:8, 0:8, "expected value")],
            [(0:6, 0:7, ArgumentKey), (0:1, 0:4, ValidFuncName)],
        );
    }

    #[test]
    fn parse_multiple_mixed_arguments() {
        p!("[val: 12pt, key=value]" =>
            [func!("val": (Pt(12.0)), { "key" => Id("value") })], [],
            [(0:12, 0:15, ArgumentKey), (0:1, 0:4, ValidFuncName)],
        );
        pval!("a , x=\"b\" , c" => (Id("a"), Id("c")), { "x" => Str("b"),  });
    }

    #[test]
    fn parse_invalid_values() {
        p!("[val: )]"     => [func!("val")], [(0:6, 0:7, "expected argument, found closing paren")]);
        p!("[val: }]"     => [func!("val")], [(0:6, 0:7, "expected argument, found closing brace")]);
        p!("[val: :]"     => [func!("val")], [(0:6, 0:7, "expected argument, found colon")]);
        p!("[val: ,]"     => [func!("val")], [(0:6, 0:7, "expected argument, found comma")]);
        p!("[val: =]"     => [func!("val")], [(0:6, 0:7, "expected argument, found equals sign")]);
        p!("[val: 🌎]"    => [func!("val")], [(0:6, 0:7, "expected argument, found invalid token")]);
        p!("[val: 12ept]" => [func!("val")], [(0:6, 0:11, "expected argument, found invalid token")]);
        p!("[val: [hi]]"  =>
            [func!("val")],
            [(0:6, 0:10, "expected argument, found function")],
            [(0:1, 0:4, ValidFuncName)],
        );
    }

    #[test]
    fn parse_invalid_key_value_pairs() {
        // Invalid keys.
        p!("[val: true=you]" =>
            [func!("val": (Bool(true), Id("you")), {})],
            [(0:10, 0:10, "expected comma"),
             (0:10, 0:11, "expected argument, found equals sign")],
            [(0:1, 0:4, ValidFuncName)],
        );

        // Unexpected equals.
        p!("[box: z=y=4]" =>
            [func!("box": (Num(4.0)), { "z" => Id("y") })],
            [(0:9, 0:9, "expected comma"),
             (0:9, 0:10, "expected argument, found equals sign")],
        );

        // Invalid colon after keyable positional argument.
        p!("[val: key:12]" =>
            [func!("val": (Id("key"), Num(12.0)), {})],
            [(0:9, 0:9, "expected comma"),
             (0:9, 0:10, "expected argument, found colon")],
            [(0:1, 0:4, ValidFuncName)],
        );

        // Invalid colon after unkeyable positional argument.
        p!("[val: true:12]" => [func!("val": (Bool(true), Num(12.0)), {})],
            [(0:10, 0:10, "expected comma"),
             (0:10, 0:11, "expected argument, found colon")],
            [(0:1, 0:4, ValidFuncName)],
        );
    }

    #[test]
    fn parse_invalid_commas() {
        // Missing commas.
        p!("[val: 1pt 1]" =>
            [func!("val": (Pt(1.0), Num(1.0)), {})],
            [(0:9, 0:9, "expected comma")],
        );
        p!(r#"[val: _"s"]"# =>
            [func!("val": (Id("_"), Str("s")), {})],
            [(0:7, 0:7, "expected comma")],
        );

        // Unexpected commas.
        p!("[val:,]" => [func!("val")], [(0:5, 0:6, "expected argument, found comma")]);
        p!("[val: key=,]" => [func!("val")], [(0:10, 0:11, "expected value, found comma")]);
        p!("[val:, true]" =>
            [func!("val": (Bool(true)), {})],
            [(0:5, 0:6, "expected argument, found comma")],
        );
    }

    #[test]
    fn parse_bodies() {
        p!("[val][Hi]" => [func!("val"; [T("Hi")])]);
        p!("[val:*][*Hi*]" =>
            [func!("val"; [Bold, T("Hi"), Bold])],
            [(0:5, 0:6, "expected argument, found star")],
        );
        // Errors in bodies.
        p!(" [val][ */ ]" =>
            [S, func!("val"; [S, S])],
            [(0:8, 0:10, "unexpected end of block comment")],
        );
    }

    #[test]
    fn parse_spanned_functions() {
        // Space before function
        p!(" [val]" =>
            [(0:0, 0:1, S), (0:1, 0:6, func!((0:1, 0:4, "val")))], [],
            [(0:2, 0:5, ValidFuncName)],
        );

        // Newline before function
        p!(" \n\r\n[val]" =>
            [(0:0, 2:0, Parbreak), (2:0, 2:5, func!((0:1, 0:4, "val")))], [],
            [(2:1, 2:4, ValidFuncName)],
        );

        // Content before function
        p!("hello [val][world] 🌎" =>
            [
                (0:0, 0:5, T("hello")),
                (0:5, 0:6, S),
                (0:6, 0:18, func!((0:1, 0:4, "val"); [(0:6, 0:11, T("world"))])),
                (0:18, 0:19, S),
                (0:19, 0:20, T("🌎"))
            ],
            [],
            [(0:7, 0:10, ValidFuncName)],
        );

        // Nested function
        p!(" [val][\nbody[ box]\n ]" =>
            [
                (0:0, 0:1, S),
                (0:1, 2:2, func!((0:1, 0:4, "val"); [
                    (0:6, 1:0, S),
                    (1:0, 1:4, T("body")),
                    (1:4, 1:10, func!((0:2, 0:5, "box"))),
                    (1:10, 2:1, S),
                ]))
            ],
            [],
            [(0:2, 0:5, ValidFuncName), (1:6, 1:9, ValidFuncName)],
        );
    }
}
